# Migration notes — agents, modes & permissions (0.6 → 0.7, before ADR-0226)

> Part of the [migration notes](../migrations.md) of the [embedding guide](../../embedding.md).

The 0.6.0 → 0.7.0 embedder breaks that predate the one-surface redesign,
diffed against `v0.6.0`'s public API. Start here: the ADR-0207 family
(profiles retired, authority moved to permission modes) is what a 0.6.0
embedder hits first. Paths are crate paths at 0.7.0; `core` is
`entanglement_core`, `runtime` is `entanglement_runtime`, `provider` is
`entanglement_provider`.

## Agents replace profiles (ADR-0207, #560)

[ADR-0207](../../adr/0207-permission-modes-replace-agent-borne-authority.md)
split a 0.6.0 profile in two: **identity** (who answers: prompt, model pin)
stays on the agent, and **authority** (what may run) moves to the session's
permission mode (next section).

- **API (core):**
  - `AgentProfile` → `Agent { name, description, system_prompt, model,
    provider, default_mode }`. Gone with it: `mode: AgentMode`, `permission`,
    `tools`, `disallowed_tools`, `can_spawn`, `spawnable_agents`, `sandbox`,
    and the methods `advertises_tool`, `mask_allows`, `may_spawn`,
    `spawn_target_allowed`, `spawnable_as_subagent` and `detail`.
    `model_pin()` stays.
  - `AgentMode` (`Primary`/`Subagent`/`All`) is removed outright. Any
    registered agent can be a spawn target, bounded by the mode's
    `max_depth`/`max_agents`.
  - `ProfileRegistry` → `AgentCatalog`, with the same
    `new`/`get`/`iter`/`insert`/`validate`. `new()` holds only the built-in
    `general` agent, which replaces `build` as the default.
  - `EngineConfig { profiles: ProfileRegistry, profile_tool_specs:
    HashMap<String, Vec<ToolSpec>> }` → `EngineConfig { agents: AgentCatalog }`.
    There is no per-agent tool table: `tool_specs` is the one surface, so
    put any spec you kept in `profile_tool_specs` into `tool_specs`.
  - `ConfigError::MissingDefaultProfile` → `MissingDefaultAgent`. It now
    requires an agent named `general` (the new public `DEFAULT_AGENT`), so a
    catalog that only holds a `build` agent fails `validate()`.
  - `SystemPromptResolver` is `Fn(&SessionId, &Agent)`, no longer
    `&AgentProfile`. `Session::new_empty(cfg, agent: Agent)`. On
    `session::Session`, `profile` → `agent` and `profile_generation` →
    `generation_by_agent`; `profile_models` is removed. It gains `mode`,
    `mode_transition_from` and `workdir`.
- **API (runtime):** `agents::built_in_registry`/`load_registry` return
  `AgentCatalog` (there is also `load_registry_in(root, &UserLayer, …)`).
  `watch::LiveDefinitions.profiles` → `agents`, and
  `AgentModelStore::apply(&mut AgentCatalog)`. Removed:
  `agents::{rewrite_tools, save_tools_override, winning_raw_text}` (the
  `tools:` frontmatter they edited is gone) and
  `propose_plan::{HANDOFF_PROFILE, specs_for, wrap_plan}`.
- **Build one:**

  ```rust,ignore
  let mut agents = AgentCatalog::new(); // `general` only
  agents.insert(Agent {
      name: "reviewer".into(),
      description: "Reviews a diff and reports findings.".into(),
      system_prompt: "You review code.".into(),
      model: None,
      provider: None,
      default_mode: Some("plan".into()), // a starting hint, never authority
  });
  // or: entanglement_runtime::agents::load_registry(root, &ctx, &skills, &mcp)?
  let cfg = EngineConfig {
      agents: agents.clone(),
      mode_ladder: mode::BUILTIN_LADDER.map(String::from).to_vec(),
      ..EngineConfig::default()
  };
  cfg.validate()?;
  ToolExecutor::builder(&holly, tools.shared()).agents(agents) /* … */;
  ```

  Pass the executor the same catalog through `.agents(AgentCatalog)`, or
  `.live_agents(Arc<RwLock<AgentCatalog>>)` if a watcher hot-swaps it
  ([§4](../tool-execution.md#constructing-the-executor--toolexecutorbuilder)).
- **Config:** agent frontmatter keeps `name`, `description`, `model`,
  `provider`, `include_brief`, `skills` and `default_mode`. A native user or
  project agent file that still names one of the retired keys (`tools`,
  `disallowed_tools`, `permission`, `mode`, `can_spawn`, `spawnable_agents`,
  `sandbox`) heals itself on load (`agents/migrate.rs`):
  - it is copied to `<name>.md.bak`;
  - it is rewritten without those keys;
  - a warning names the file, the dropped keys, the backup and the closest
    mode (`research` for a `disallowed_tools` list, a `permission` written
    as a string containing `deny`, or a `tools` list with no write/exec
    tool, else `build`; a `permission:` mapping such as `default: deny` is
    not inspected).

  Any other unknown key still fails the load (`deny_unknown_fields`), also
  after the retired keys are dropped. Cross-vendor agent files read only
  `name`/`description` and ignore the rest. The built-in roster is
  `general`/`plan`/`debug`. A 0.6.0 log naming `build` or `explore` resumes
  as `general`
  ([ADR-0214](../../adr/0214-retired-agent-logs-resume-as-general.md)).

## Permission modes carry authority (ADR-0207/0209/0218, #560, #763)

- **Concept:** a session is an `(agent, mode)` pair. The four modes
  `research`/`plan`/`build`/`auto` live in the runtime's `mode` module:
  `ModeTable::builtin()`, `ModeTable::new(Vec<Mode>)`, `Mode::resolve(tool,
  capabilities, arg, workdir)`, `mode::BUILTIN_LADDER` (narrowest →
  widest), and core's `DEFAULT_MODE` (`"build"`).
- **API (runtime):**
  - `policy::ProfileResolver` → `ModeResolver::new(modes, table, registry,
    base, root)`. It reads the session → mode map the executor folds
    `ModeChanged` into. `ProfileResolver::with_live_bash` is gone.
  - `ProfileSandboxResolver` → `ModeSandboxResolver::new(modes, table,
    base)`. `SandboxConfig::from_env()` and its `own`/`floor` fields →
    `SandboxConfig::new(modes, table)`.
  - Removed mask helpers: `permission::{effective_permission, tool_masked,
    tool_mask_source, skill_masked, ActiveSkill}`.
    `clamp_to_base` gains a `capabilities: &[Capability]` argument.
  - `Tool` gains `fn capabilities(&self) -> &'static [Capability]`. The
    default is `[Capability::Write]`, so override it on a read-only custom
    tool: `plan` and `research` deny the `write` class.
  - `system_prompt::assemble`/`assemble_parts` lose their `mode: AgentMode`
    argument.
- **API (core):** `EngineConfig` gains `mode_ladder: Vec<String>` and
  `modes_preamble: Option<String>`. Set the ladder to `BUILTIN_LADDER`.
  It is empty by default, and then a root's `Spawn.mode` is carried
  unvalidated, a child always inherits its parent's mode, and an agent's
  `default_mode` hint never applies. For the preamble,
  `skutter` sets `mode::describe::modes_preamble()`.
- **Behaviour:** the 0.6.0 `build` profile allowed everything. The `build`
  mode (`mode/builtin/build.yml`) works like this:
  - It allows the `read`, `control`, `write` and `exec` classes.
  - It denies `plan` and destructive bash (`rm -rf /*`, `rm -rf ~*`,
    `git push --force`/`-f`, `git reset --hard`, `git clean -f`,
    `git branch -D`).
  - It prompts for `rhai` and for push/publish/PR commands (`git push`,
    `gh`/`glab` PR, MR, issue, release and repo creation or merge,
    `npm publish`, `cargo publish`).
  - Any other tool gets its `default: prompt`.

  `auto` allows all but its deny/prompt lists
  ([ADR-0209](../../adr/0209-auto-mode-defaults-to-prompt-not-deny.md)).
- **Porting a custom profile's `permission:`:** turn the 0.6.0
  `PermissionProfile` into a mode of your own:

  ```rust,ignore
  // `permission`: the PermissionProfile your 0.6.0 AgentProfile carried
  let mut custom = Mode::from_permission_profile(&permission);
  custom.name = "tenant".into();
  custom.limits = Limits { max_depth: Some(2), max_agents: Some(4), ..Limits::default() };
  let builtin = ModeTable::builtin()?;
  let mut modes: Vec<Mode> =
      BUILTIN_LADDER.iter().filter_map(|n| builtin.get(n).cloned()).collect();
  modes.push(custom); // ModeTable::new replaces the built-ins: keep the ones you want
  let table = ModeTable::new(modes)?;
  cfg.mode_ladder = vec!["research".into(), "plan".into(), "tenant".into(),
                         "build".into(), "auto".into()]; // narrowest → widest
  ToolExecutor::builder(&holly, tools.shared()).mode_table(Arc::new(table)) /* … */;
  // then start sessions in it: Spawn { mode: Some("tenant".into()), .. } or SetMode
  ```

  Place the new name on `EngineConfig.mode_ladder` where its breadth
  belongs: the ladder decides what a child or a `default_mode` hint may
  start in. An agent *file's* `default_mode` may name only a built-in mode
  up to `build` (any other is warned and ignored), and `request_mode` moves
  only along the built-in ladder. Put sessions in a custom mode with
  `Spawn.mode`/`SetMode`, or with `default_mode` on an `Agent` you build in
  code.

## The agent is fixed for a session (ADR-0207 §9, #725)

- **Wire:** `InMsg::SetAgent { session, agent }` is removed. Pick the agent
  in `Spawn.agent`. To change what a session may do, send `InMsg::SetMode
  { session, mode }`. To start a session configured, use the new
  serde-defaulted `Spawn` fields: `mode: Option<String>`,
  `model: Option<ModelChoice>`, `workdir: Option<String>`. An unresolvable
  `model` or `workdir` refuses the spawn with an `Error`. So does a `mode`
  missing from `EngineConfig.mode_ladder`, but only when the ladder is
  non-empty.
- **Wire:** `SessionStarted.profile` and `SessionInfo.profile` → `agent`. A
  `#[serde(alias = "profile")]` keeps 0.6.0 frames and logs loadable.
  `AgentChanged.profile_detail`, `SessionInfo.profile_detail` and the
  `ProfileDetail` type are gone.
- **Behaviour:** a `Spawn` with an empty `prompt` binds the session and
  starts it idle, queueing no turn (#674). Send the real `Prompt` after it.
- **Exhaustive matches** over `InMsg` lose the `SetAgent` arm.

## Mode-scoped grants (ADR-0207 §8, #634)

- **API:** `GrantStore::is_granted`, `record` and `grant_session_dir` take
  the mode the approval was earned in (`runtime::policy`):

  ```rust,ignore
  fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool;
  async fn record(&self, session: &SessionId, tool: &str, arg: Option<&str>,
                  scope: ApprovalScope, mode: &str);
  fn forget_session(&self, session: &SessionId);
  fn grant_session_dir(&self, session: &SessionId, dir: &str, mode: &str) -> String; // default: echoes `dir`
  ```

  The executor passes the mode the call was graded under, and calls `record`
  for every non-`Once` approval. If you call a store directly, pass the
  mode the session was in when the card was answered, and key
  `Session`/`Always` grants by it. `FileGrantStore`'s methods gained the
  same parameter.
- **Behaviour:** a grant matches only its own mode, so a grant from `build`
  is inert in `research`. A pre-0.7 `grants.yml` entry has no mode and
  matches nothing (it asks again, the safe direction). The session's tool
  overlay is dropped on a mode change.

## `Approve` carries the plan choice (ADR-0207/0228)

- **Wire:** `InMsg::Approve` gains `mode: Option<String>` (the mode an
  accepted `propose_plan` switches to; `None` means `auto`) and `run_all: bool` (the "Run all"
  answer to a script launch card). Both are serde-defaulted, but a struct
  literal must set them: `mode: None, run_all: false`.
- **API:** `seam::Decision::Approve { scope }` gains the same two fields.

## Spawn tool specs (ADR-0207 stage 5, ADR-0226)

- **API:** `subagent::spawn_specs_for(profile, registry)` →
  `subagent::agent_specs() -> Vec<ToolSpec>` (the `agent` and `agent_send`
  specs, the same for every session). `subagent::agent_spec(targets)` →
  `agent_spec()`. `permission::spawn_refusal(spawner, target, registry)` →
  `spawn_refusal(target, &AgentCatalog)`. `SpawnGuard::try_spawn(parent)` →
  `try_spawn(parent, &Mode) -> Result<SpawnSlot, String>`, and the
  `*_sponsored*` methods are gone. Removed: `subagent::agent_spawn_spec`.

## One `poll`, no live bash toggle (ADR-0161/0163, #606/#607)

- **Wire:** `InMsg::BashEnable`/`BashDisable` and `OutEvent::BashChanged`
  are removed (bash enablement is a `SetToolOverlay` entry). Core drops
  `BashGrade` and `Holly::emit_bash_changed`.
- **API (runtime):** removed modules `agent_poll` (`AgentRegistry` and
  `AgentStatus` move to `agent_registry`), `bash_live` and
  `host::bash_output` (`BashOutputTool`). Removed `tool_names::{AGENT_POLL_TOOL,
  AGENT_SPAWN_TOOL, MULTI_GROUP}`. `JobRegistry::spawn` gains `timeout` and
  `owner`; `JobRegistry::poll` is async and takes the caller and a timeout.

## Structured tool results (ADR-0176/0186, #636, #681)

- **Wire/API:** `InMsg::ToolResult` and `OutEvent::ToolOutput` gain
  `is_error: bool`, `duration_ms: Option<u64>` and `exit_code:
  Option<i32>`. `Holly::submit_tool_result(session, request_id, content)` →
  `submit_tool_result(session, request_id, content, is_error, duration_ms,
  exit_code)`.
- **API (runtime):** `ToolRegistry::execute` returns `ToolExecution`, not
  `Vec<ContentPart>`. `Tool::run_with_meta` is new, and its default wraps
  `run_for_session`. `seam::reply` gains `is_error`;
  `reply_content` gains all three fields.
  `Hooks::run_post_tool_use` gains `is_error, exit_code`.

## Provider structs and factories (ADR-0160, ADR-0189, #673)

- **API:** `LlmRequest` gains `cache_key: Option<&str>`, `retry:
  Option<RetryConfig>` and `trailing_notice: Option<String>`. It has no
  `Default`, so a literal sets all three (`None` keeps 0.6.0 behaviour).
  `LlmResponse` gains `reasoning`, `content_blocks`, `stop_reason` and
  `usage`. It is `Default`, so finish a literal with `..Default::default()`.
  Its module path is now `response::` (the crate-root re-export is
  unchanged).
- **API:** the factories and constructors take new positional arguments.
  `openai_factory(base, key, auth, model, rpm, concurrency,
  model_concurrency, web_search, send_prompt_cache_key, thinking_spec,
  http)`, `anthropic_factory(…, auth, …, model_spec, http)` and
  `gemini_factory(…, auth, …)`: pass `auth: None` for a static key,
  `send_prompt_cache_key: false`,
  `fixed_thinking_spec(ThinkingSpec::default())` and
  `AnthropicModelSpec::default()`. `OpenAiLlm::new`/`AnthropicLlm::new`
  take the same additions, except `auth`. `HttpClient::execute_with_retry`
  gains `retry: Option<RetryConfig>`.
- **API (core):** `ToolSpecResolver` is `Fn(&SessionId, SessionModel<'_>)`.
  The second argument is the session's bound provider/model.

## The OAuth stack moved to `provider::oauth` (ADR-0189, #684)

- **API:** `provider::mcp::auth::*` → `provider::oauth::*`, with no
  compatibility re-export. The `mcp::{AuthFlow, AuthOutcome, AuthRequired,
  OauthConfig, StoredAuth, TokenSet, TokenStore, AccessTokenSource}`
  re-exports moved to the provider crate root. Core's root re-exports keep
  their names, so `entanglement_core::TokenStore` still works.
  `mcp::ClientRegistration` → `oauth::ClientRegistration`.
  `oauth::check` takes `store: Arc<dyn TokenStore>` instead of
  `&dyn TokenStore` (`disconnect` still takes `&dyn TokenStore`). `dcr::register` takes `redirect_uri: Option<&str>,
  grant_types: &[&str]`, and `discovery::discover` gains
  `override_device_authorization`.

## Env vars pruned (ADR-0213, #739)

- **Env:** set these differently when you configure the process
  programmatically:
  - `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE` → `ENTANGLEMENT_DEBUG=no-shared-state`
  - `ENTANGLEMENT_LOG_BODIES` → `ENTANGLEMENT_DEBUG=bodies`
  - `ENTANGLEMENT_ECHO_FULL` → `ENTANGLEMENT_DEBUG=echo`
  - the six managed-state `ENTANGLEMENT_*_FILE` overrides (`GRANTS`,
    `AGENT_MODELS`, `AGENT_GENERATION`, `AUX_MODELS`, `EXTRA_ROOTS`,
    `MCP_TOKENS`) → one `ENTANGLEMENT_STATE_DIR`
  - `{NAME}_BASE` → `{NAME}_API_BASE`
  - `{NAME}_RPM`/`{NAME}_CONCURRENCY`/`ENTANGLEMENT_MAX_CONCURRENCY` →
    `rpm:`/`concurrency:` in `providers.yml`
  - `ENTANGLEMENT_SESSION_RETENTION_DAYS` and `ENTANGLEMENT_TUI_*` →
    `config.yml` keys
  - `ENTANGLEMENT_ENABLE_BASH` → nothing (bash is always registered)

  Read the debug flags with `DebugFlags::from_env()` (re-exported by core).
- **Behaviour:** a retired variable is ignored. `skutter` prints one warning
  per retired variable that is still set. An embedder gets the same lines
  from `runtime::config::retired::process_env_warnings(&provider_names)`;
  nothing warns unless you call it.

## `ToolExecutor::builder` (#724, #747/#775)

- **API:** `ToolExecutor::builder(&holly, tools.shared())` replaces
  `spawn_tool_executor{,_with_hooks,_with_policy}`. Those are deprecated
  shims, and they now take `AgentCatalog` where 0.6.0 took
  `ProfileRegistry`. `_with_policy` lost its `sandbox` argument and gained
  `jobs`/`retained`/`scripts`/`perm_modes`/`mode_table`/`plan_files`/
  `mcp_scopes`/`catalog`/`discovery`. The full mapping is in
  [§4](../tool-execution.md#constructing-the-executor--toolexecutorbuilder).
- **Behaviour:** the 0.6.0 `spawn_tool_executor`/`_with_hooks` persisted
  `Always` grants to `grants.yml`. Those shims and the builder now keep
  grants in memory unless you pass
  `.grants(Arc::new(DefaultGrantStore::load()))`. `_with_policy` still uses
  the store you hand it, as in 0.6.0; its `active` map is now
  `Arc<Mutex<HashMap<SessionId, Agent>>>`.

## `SessionStore` replaces cwd-keyed free functions (#747, #773)

- **API:** the `session_store::{append, read, delete, list_sessions,
  prune, session_path, scratch_dir}(cwd, …)` free functions → methods on
  `SessionStore` (`SessionStore::for_cwd(cwd)`, or `SessionStore::at(dir)`
  for an explicit directory). `session_dir(cwd)` →
  `SessionStore::for_cwd(cwd).dir()`. `base_dir()` is removed.
  `persistence::FileSink::new`, `spawn_persistence_subscriber` and
  `history::spawn_history_responder` take a `SessionStore` instead of a
  `cwd: PathBuf`. Loaders that read the user layer (`load_layers`,
  `inspect::*`, `skill_detail`) take a `&UserLayer`.

## `multi_user` leaves the runtime (ADR-0181/0184/0188, #686/#687/#684)

- **API:** `runtime::multi_user` is gone: `SessionUserRegistry`,
  `PerUserPermissionResolver`, `PerUserGrantStore`, and `multi_user::provider`
  (`UserProviderStore`, `InMemoryUserProviderStore`, `UserProviderContext`,
  `build_user_model_resolver`). The provider seams moved unchanged to
  `provider::multi_user` (re-exported at the crate root). For permissions
  and grants, implement `PermissionResolver`/`GrantStore` over your own
  session → user map. MCP credentials use `oauth::UserTokenStore` +
  `user_scoped(store, user)`. Recipe:
  [§7](../approvals-mcp-multi-user.md#7-multi-user-per-user-providers-permissions-tokens).

## Smaller changes

- **Core:** `Context::apply_compaction` is removed (every compaction forks,
  ADR-0205). `TurnState::begin_batch`/`resolve` carry `ToolEnvelope`s.
  `Session.seq` is an `Arc<SeqCell>`. The `McpServerState` re-export is
  removed (ADR-0226).
- **Runtime:** `serve::router`/`serve` gain `attendance, rejoin`
  ([ADR-0247](cards-and-sessions.md#attended-sessions-wait-for-their-approver-adr-0247-828)).
  `PendingDecisions::register` takes `kind, detail`, and `resolve` returns
  a `Resolution`. `McpTool::new` takes a capabilities map.
  `env_date::date_resolver`, `script::{run_rhai, BindingPolicy}`,
  `host::CallTool::{with_scratch_base, with_bash_status}` and
  `plan_tasks::explicitly_allowlists` are removed. `ask_user`/`propose_plan`/`subagent` runners take extra
  arguments.
- **Unchanged:** `PermissionResolver::resolve(session, tool, input)` and
  the rest of the `Tool` trait. `RecordSink` only gains the defaulted
  `public_error`/`touch`. `InMsg`/`OutEvent` frames change only by the
  fields listed here and on the later pages.

---

Previous: [§5–§7 — Approvals across restart, MCP & multi-user](../approvals-mcp-multi-user.md) · Next: [Migration notes — one surface, binding path & skills (ADR-0226–0230)](one-surface-and-rhai.md)
