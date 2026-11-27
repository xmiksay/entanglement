# Migration notes — 0.7 → 0.8: crate split (#931, #964, #965, #966, #962, #967)

> Part of the [0.7 → 0.8 migration notes](../0.7-to-0.8.md).

## The `skutter` crate takes the binary and the heads (#931)

`entanglement-runtime` is a library only. The `skutter` binary and every
head moved to the new workspace crate `skutter`: the startup wiring, the
`run`/`pipe` stdio heads, the TUI, the WebSocket `serve` head, user slash
commands, `inspect`, the log setup and `config set-key`. `skutter` is
`publish = false`: the release publishes the three library crates only
([releasing](../../../releasing.md)). Neither site (`default-features = false`,
`mcp-http` + `provider`) nor nexial (`default-features = false`, `mcp-http`)
names anything below. What changes:

- **Removed binary:** `cargo install entanglement-runtime` no longer installs
  `skutter`. Install it from a checkout: `make install`
  (`cargo install --path skutter --locked`).
- **Removed features:** `cli`, `tui` and `serve`. The runtime's default
  features are now `provider`, `mcp-http`, `rhai` and `web-fetch`, and no
  feature pulls `clap`, `axum`, `crossterm`, `ratatui` or the TUI render
  stack (`make check-lean` gates the default build as well as
  `--no-default-features`).
- **Moved to `skutter` (no longer runtime API):** `serve` (`router`,
  `serve`, `Rejoin`, …), `inspect`, `slash_commands`, `logging`, and
  `config::keys` (`set_key`).
- **Changed API:** `watch::LiveDefinitions::new` drops its `commands`
  argument and `LiveDefinitions` its `commands` field: the runtime no longer
  knows about user slash commands. The watcher still fingerprints the
  layered `commands` dirs, so a head that keeps its own commands reloads
  them on the watcher's `ReloadNotice` (what `skutter`'s TUI does). The
  reload summary no longer counts commands. The env var that relocates
  the user commands layer is `watch::COMMANDS_DIR_ENV`, so a head loads
  the same dirs the watcher fingerprints.
- **Now public** (needed by `skutter`'s `inspect`): `mcp::transport_label`,
  `mode::PRECEDENCE`, `mode::Rules::iter`, `layers::walk_files`,
  `skills::load_skill::render_skill`.
- **Instead:** an embedder that drove the WebSocket head or the slash
  command registry depends on `skutter` by path or git (it is not on
  crates.io), or keeps its own copy. Drop `cli`/`tui`/`serve` from any
  `entanglement-runtime` feature list; drop the `commands` argument from
  `LiveDefinitions::new`.

## Hook execution out of the library executor (#964)

The library runs no hooks. Lifecycle hooks moved to `skutter`, which runs
them from the seams any embedder has: a blocking `pre_tool_use` is a
`PermissionResolver` wrapper (`skutter::hooks::HookResolver`), every other
point a subscriber on the `InMsg`/`OutEvent` fan-out
(`skutter::hooks::spawn`). No new trait replaces them; a resolver's `Deny`
can now carry its reason. Site passes `Hooks::default()` to
`spawn_tool_executor_with_policy` and nexial names no hook item; both
implement `PermissionResolver`, so both change its return type. What
changes:

- **Removed API:** the `entanglement_runtime::hooks` module (`Hooks`,
  `HookSpec`, `HookEvent`); `ToolExecutorBuilder::hooks`;
  `tool_runner::spawn_tool_executor_with_hooks`; the `hooks` argument of
  `tool_runner::spawn_tool_executor_with_policy`.
- **Changed API:** `config::Config::hooks` is the `hooks:` section as raw
  `serde_yaml::Value` (`Null` when absent), no longer a parsed `Hooks`: the
  library no longer validates it, so a typo in it is no longer a config
  load error in the library.
- **Changed API:** `PermissionResolver::resolve` and
  `resolve_with_capabilities` return `policy::Grade` (`Allow | Ask |
  Deny(Option<String>)`), not `Permission`. A `Deny(Some(reason))` reaches
  the model verbatim as the call's error result, in place of the
  `denied by mode … use /mode to switch` text, which a `Deny(None)` still
  gets. A resolver that grades by mode alone returns `permission.into()`;
  a vetoing one returns `Grade::deny(reason)`. `tool_runner::grade_in_scope`
  returns `(Grade, mode)`, so a redirect hop's refusal carries the reason too.
- **Changed behaviour (`skutter`, signed off on #964):** a `pre_tool_use`
  veto grades the call `Deny` with the hook's reason, so the model gets the
  same `tool X blocked by pre_tool_use hook: …` text as in 0.7. The hooks
  run wherever the resolver is asked (`propose_plan` and an in-call
  redirect hop included). Known limitation until #936: a sub-agent's call
  runs them once per session in its ancestor chain, each run naming that
  ancestor. They match the graded tool name (`read_raw` → `read`,
  `glob_json` → `glob`, `grep_json` → `grep`, a `call_grading` target), so a
  `tools: [read_raw]` filter never matches. `post_tool_use` is observe-only
  telemetry: it fires on the `ToolOutput` of a host-tool call the runtime ran
  (`ToolExec`, a measured `duration_ms`) — not a refused call, not a
  script's nested calls, not the executor's own routes (`agent`, `poll`,
  `ask_user`, … — as in 0.7) — and runs asynchronously after the result
  folded back instead of before, on its own worker, so a formatter hook
  races the agent's next call on the same file. Synchronous post hooks run
  inside `skutter`'s own tools, script calls included, once host tools
  (#968) and the file/exec bindings (#933) move there. Its `input` is still
  the workdir-rewritten one (`path_grading::rewrite`, now public).
  `user_prompt_submit` runs on the first event a prompt causes. Details:
  [hooks §9](../../../architecture/gates/hooks-and-seams.md).
- **Instead:** drop `Hooks::default()` from `spawn_tool_executor_with_policy`
  calls and any `.hooks(..)` builder call. An embedder that wants a veto
  wraps its resolver the way `HookResolver` does; one that wants to observe
  calls subscribes to `ToolExec`/`ToolOutput` itself.

## The advertised surface is a per-tool flag (#965)

The hard-coded `KERNEL` name list is gone. A registered tool is advertised
when its new defaulted `Tool::advertised` returns `true` (default `false`);
the executor's own routes (`discover::runtime_owned_specs`,
`subagent::agent_specs`) are always advertised. The built-in tools that were
`KERNEL` members (`read`, `edit`, `write`, `apply_patch`, `bash`,
`web_fetch`, `load_skill`) set it, so `skutter`'s first-request surface is
byte-identical to 0.7's. Neither site nor nexial names `KERNEL` or
`build_surface`, and neither installs `tool_advertising::surface`'s
resolver. What changes:

- **Removed API:** `tool_names::KERNEL`.
- **Changed API:** `tool_advertising::surface::build_surface` takes the
  `&ToolRegistry` instead of a `Vec<ToolSpec>` snapshot of its specs, so it
  can ask each tool for the flag.
- **Changed behaviour:** a tool is no longer advertised because of its
  name. An embedder tool registered as `read`, `bash`, … under the library's
  surface resolver was advertised in 0.7 and is not now unless it sets
  `advertised`; any other embedder tool can now join the surface.
- **Instead:** override `fn advertised(&self) -> bool { true }` on each
  tool the model should see directly; pass `&registry` to `build_surface`.

## File-backed storage moves to `skutter` (#966)

The library's policy and MCP modules no longer touch the filesystem or the
config layer. Their defaults are in memory; `skutter` plugs in the managed
files (`grants.yml`, `mcp-tokens.yml`, `mcp-tools.yml`, the `mcp:` section of
`config.yml`) through the existing seams. Site and nexial implement
`GrantStore`/`PermissionResolver` themselves and name none of the items
below. What changes:

- **Removed API:** the `entanglement_runtime::grants` module
  (`FileGrantStore`, `GrantKey`); `policy::DefaultGrantStore`;
  `config::McpTokenStore` (`config::mcp_tokens`); `config::save_mcp`
  (`config::mcp_persist`); the `mcp::McpToolCache` struct with
  `managed()`/`at()`.
- **New API:** `policy::InMemoryGrantStore` (the executor builder's default,
  as before); `mcp::McpToolCache` is now a trait (`lookup`, `record`) with
  `mcp::InMemoryMcpToolCache`; `mcp::InMemoryTokenStore`;
  `AvailableMcp::token_store`.
- **Changed API:** `AvailableMcp::with_tool_cache` takes
  `Arc<dyn McpToolCache>` and `with_token_store` `Arc<dyn TokenStore>`;
  `tool_cache()` returns `&Arc<dyn McpToolCache>`. `mcp::oauth_ops::run`
  takes the `Arc<dyn TokenStore>` to save, check and revoke in, and
  `mcp::live::mcp_reconnect` and `mcp::live::mcp_add` the one to connect
  with (an `oauth:` server's bearer; 0.7's `mcp_add` read the managed file).
  `watch::LiveDefinitions::new` drops its `grants` argument and
  `LiveDefinitions` its `grants` field; a reload no longer re-reads grants.
- **Changed behaviour (library):** an `AvailableMcp` built without
  `with_token_store` keeps OAuth credentials in memory — 0.7 read and wrote
  the managed `mcp-tokens.yml` by default. Without `with_tool_cache` the
  last-seen tool names last for the process (0.7's default cache was inert).
  `mcp::live::mcp_add`/`mcp_remove` change only the in-memory
  `ServerConfigs`; a config write failure can no longer fail an add or
  remove.
- **Changed behaviour (`skutter`):** `skutter::state::FileGrantStore`
  re-reads `grants.yml` whenever the file changed, so a grant another
  instance recorded, or a line deleted by hand, counts at once instead of at
  the next watcher reload; the definitions watcher no longer reloads on a
  `grants.yml` write. `skutter::state::spawn_mcp_config_writer` writes
  `config.yml`'s `mcp:` section when a successful `OutEvent::McpChanged`
  changed the server set; a write failure still reaches the heads as an
  `McpChanged` error (now a second event, after the success).
- **Instead:** a head that wants grants, MCP credentials or last-seen tool
  names to outlive the process implements `GrantStore`, `TokenStore` or
  `McpToolCache` over its own storage and passes it to
  `ToolExecutor::builder().grants(..)` / `AvailableMcp::with_token_store(..)`
  / `with_tool_cache(..)`; one that wants a live `/mcp add|remove` to survive
  a restart persists `ServerConfigs` on `OutEvent::McpChanged`. Drop the
  `grants` argument from `LiveDefinitions::new`.

## Permission arguments come from the tool (#962)

The permission pipeline no longer knows any host tool by name. What an
argument-scoped `tool(pattern)` rule (and an `Always` grant) matches, and
whether a call carries a workdir for a `tool{pattern}` rule, is the
registered tool's own answer — as the workdir rewrite and the escape-root
target already were (#930). Site and nexial call none of the changed
functions (site's resolver has its own `permission_arg` analog).

- **Removed API:** `permission_path::is_path_arg_tool`,
  `permission_path::rooted_arg` (and its name list `PATH_ARG_TOOLS`).
- **New API:** the defaulted trait methods `Tool::permission_scope()`
  (a `tools::PermissionScope { arg: ArgKind, workdir: bool }`, where
  `ArgKind` is `None`/`Path`/`Command`/`Text`) and
  `Tool::permission_arg(input)`; `permission::permission_scope(&tools,
  tool)`; `permission_path::rooted_path(root, path)` (lexical) and
  `rooted_script(root, path)` (canonical, what `rhai` grades on);
  `config::ScopedRule`, `Config::scoped_rules` and
  `Config::warn_unscopable_rules(&tools)`.
- **Changed API:** `permission::permission_arg`,
  `permission::permission_workdir` and `permission_path::grading_arg` take
  the `&ToolRegistry` first. Only the executor's own routes (`rhai`,
  `agent`, `agent_send`) are answered without it.
- **Changed behaviour:** an embedder tool registered under a host name
  (`read`, `bash`, …) no longer gets that tool's argument, root-relative
  grading, workdir scope or approval-card root line by name; any tool that
  implements the two methods gets them, so an embedder tool can now be the
  target of `tool(path)` rules. A config rule whose *scope* can never match
  (`mcp__gh__x(foo)`, `read{/tmp/*}`, `bash(git *){/x}`) no longer warns at
  config load: the name check stays there, the scope check runs where the
  registry is known (`skutter` calls `warn_unscopable_rules` once the tool
  executor is built, since the builder registers `http` at spawn).
  `skutter inspect` and the TUI's config reload run no engine, so they do
  not repeat the scope warnings — look for them in the startup log. The
  `skutter` TUI and `run` headers and a script's nested-call
  lines take the argument from the registry the same way; a `glob` card
  shows its base `path` on its own line.
- **Instead:** implement `permission_scope`/`permission_arg` on a tool
  whose calls should be scopable (`ArgKind::Path` for a filesystem path,
  graded root-relative; `Command` for a command line; `Text` otherwise;
  `workdir: true` when its input carries a top-level string field named
  exactly `workdir` — the field name is fixed). A `Path` tool's
  `permission_arg` defaults to its `escape_target`, so override it only
  when the graded string differs. Pass the registry to
  `permission_arg`/`permission_workdir`/`grading_arg`, and call
  `Config::warn_unscopable_rules` once your registry is complete (after the
  executor builder's `spawn`, which registers `http`) if you load `Config`.
  A custom `PermissionResolver` must read the live `SharedRegistry` (the
  handle passed to `ToolExecutor::builder`, read-locked per call as
  `policy::ModeResolver` does), not a `ToolRegistry` clone: the builder
  registers `http` at spawn and lazy MCP tools register later, so a clone
  never scopes their rules.

## Config, managed files and live reload move to `skutter` (#967)

The library reads no config file, no env file and no user directory: the
embedder hands it plain values. The layered `config.yml`, the managed pin
files (`agent-models.yml`, `agent-generation.yml`, `aux-models.yml`,
`extra-roots.yml`), the state-dir resolution, the definitions watcher and
the key writer are `skutter`'s now (`skutter::config`, `skutter::watch`,
`skutter::state`). Site and nexial name none of the items below (both use
`session_store` records and `SkillRegistry::default()` only).

- **Removed API:** the whole `entanglement_runtime::config` module —
  `Config`, `Resolved`, `ConfigLayer`, `ScopedRule`, `TuiConfig`,
  `scaffold_if_missing`, `user_config_path` and the submodules
  `agent_generation`, `agent_models`, `atomic`, `aux_models`, `env_file`,
  `env_key`, `http_auth`, `keybindings`, `keychord`, `lock`, `state_dir`,
  `statusline`, `theme`, `tui`, `undo`, `write_key`; `watch::spawn_watcher`,
  `watch::LiveDefinitions`, `watch::ReloadNotice`, `watch::COMMANDS_DIR_ENV`;
  `ExtraRootStore::load`/`load_from`; `layers::UserLayer::FromEnv`;
  `agents::load_registry`/`resolve_registry`/`prompt_report` and
  `skills::load_registry`/`resolve_registry` (the env-reading wrappers);
  `system_prompt::PromptContext::load`; `AuxModelStore::load`/`load_from`/
  `reload`; `aux_llm::Purpose`'s `Serialize`/`Deserialize` (key it with
  `as_str`/`parse`). The runtime drops its `dirs` and `fd-lock`
  dependencies.
- **Moved API:** `config::http_auth::{HttpAuth, HttpAuthEntry, ResolvedAuth,
  ProjectHttpRules}` → `http_auth::…` (plain data, unchanged);
  `config::aux_models::{AuxModelStore, Purpose}` → `aux_llm::…`;
  `config::undo::format_size` → `file_backup::format_size`.
- **New API:** `extra_roots::AlwaysRoots` (`load`, `add`) and
  `ExtraRootStore::with_always_roots(Arc<dyn AlwaysRoots>)`;
  `layers::UserLayer::Empty` (the default) and `UserLayer::Home { home,
  config }`; `layers::candidate_dirs(root, kind, &UserLayer)` is public;
  `watch::spawn_debounced_watcher` is public (the one watch primitive left);
  `mode::capability_class`, `mode::rule_tool_name`
  and `mode::deserialize_grade` are public (for an embedder's own config
  checks).
- **Serde rule:** a plain data type the embedder hands the library keeps
  its `Deserialize` and field grammar, so an embedder can read it from its
  own config format; the library itself opens no file. That covers
  `BackupLimits` (sizes as bytes or e.g. `8MiB`, unchanged from 0.7),
  `HttpAuth`/`HttpAuthEntry` and the mode grades (`mode::deserialize_grade`).
  A derive nothing deserializes through is dropped (`Purpose`).
- **Changed API:** `AuxModelStore` is in memory: `set` is infallible and
  writes nothing. `layers::load_layers` drops its `env` argument.
  `SessionStore::for_cwd(base, cwd)` takes the base dir
  (`<base>/<safe_cwd_name(cwd)>`; `skutter`'s base is
  `<data_dir>/entanglement/sessions`).
- **Changed behaviour (library):** an `ExtraRootStore` without
  `with_always_roots` keeps `Always` escape-root grants for the process —
  0.7 read and wrote `extra-roots.yml` by default. A loader given
  `UserLayer::default()` reads no user layer; 0.7's `FromEnv` read
  `ENTANGLEMENT_AGENTS_DIR`/`ENTANGLEMENT_SKILLS_DIR` or `~/.claude/<kind>`
  and `${config_dir}/entanglement/<kind>`. The library reads none of
  `ENTANGLEMENT_CONFIG_FILE`, `ENTANGLEMENT_ENV_FILE`,
  `ENTANGLEMENT_STATE_DIR`, `ENTANGLEMENT_*_DIR`. It still reads the process
  env for `ENTANGLEMENT_PREAMBLE_FILE`/`ENTANGLEMENT_BRIEF_FILE` (#975) and
  an `http_auth` entry's `${VAR}` secrets (#976); each is a follow-up.
  (`ENTANGLEMENT_SANDBOX*` left with the sandbox, #968.)
- **Changed behaviour (`skutter`):** every file, env var and reload works as
  in 0.7; the warning for an `ENTANGLEMENT_*_DIR` that points at a missing
  dir names the kind and the path, no longer the env var.
- **Instead:** pass `UserLayer::Dir(path)` or `UserLayer::Home { home,
  config }` to the `*_in` loaders; build `PromptContext::load_with(root,
  &sessions)` and `SessionStore::at(dir)` (or `for_cwd(base, cwd)`); fill an
  `AuxModelStore::in_memory(pins)` from your own storage; implement
  `AlwaysRoots` over your storage to keep `Always` escape-root grants; build
  `BackupLimits`, `HttpAuth` and `TrustedFolders` from your own config; call
  `watch::spawn_debounced_watcher` for your own reloads. The scope warnings
  of the previous section are `skutter::config::Config`'s now.

## Host tools, sandbox and escape-root move to `skutter` (#968)

The library ships no host tool and keeps no filesystem-root logic: the
file, search, shell and fetch tools, their containment, the bubblewrap
sandbox, the `/cd` resolver and the escape-root check are `skutter`'s
(`skutter::host`, `skutter::state::ExtraRootStore`). The grading pipeline
names no host tool. Site and nexial use none of the moved items; nexial's
`Mode::resolve` calls gain the argument kind.

- **Removed API:** the `host` module (`ReadTool`…`WebFetchTool`,
  `host_tools`/`host_tools_with`/`host_tools_with_extra_roots`,
  `HostToolOptions`, `sandbox::*`, `list_files`/`list_files_with_extra_roots`,
  `FileList`, `resolve_under_root_or_grant`, `resolve_workdir*`,
  `unified_diff`), the `extra_roots` module (`ExtraRootStore`, the
  `AlwaysRoots` seam #967 added), the `workdir` module (`resolver`,
  `resolve`), `path_grading::{escape_target, escaping_path,
  resolve_under_root, resolve_and_contained}`, `tool_runner::EscapeRoot`
  and `ToolExecutorBuilder::escape_root`, `capability::static_capability_of`,
  `tool_names::{known_tool_names, is_recognized_mask_entry}`,
  `tool_state::static_roster`, and the `web-fetch` feature (with the
  `glob`, `ignore`, `base64` and HTML-converter dependencies).
- **New API:** `ToolExecutorBuilder::root(PathBuf)` — the project root a
  path argument grades relative to and plans live under;
  `policy::AskReason { text, card, grant }`, `policy::AskGrant { tool,
  arg }`, `Grade::ask(text, card)` and `Grade::ask_keyed(text, card,
  grant)`;
  `Tool::script_unbounded()` (default `false`: a script's nested call is
  cut at the script's budget); `tool_names::LIBRARY_TOOL_NAMES`;
  `tools::text_parts` is public.
- **Changed API:** `Grade::Ask` carries `Option<AskReason>` (match
  `Grade::Ask(_)`); `Mode::resolve`, `mode::rules::resolve` and
  `permission::clamp_to_base` take the call's `ArgKind` after the argument;
  the deprecated `spawn_tool_executor_with_policy` takes `root:
  Option<PathBuf>` where it took an `EscapeRoot`; `NON_MASKABLE_TOOLS`
  drops `read_raw`/`glob_json`/`grep_json`.
- **Changed behaviour (library):** a command argument splits per compound
  segment when the tool declares `ArgKind::Command` (0.7: when it was named
  `bash`). The trusted-script write floor covers any registered tool with
  `Capability::Write` and a path argument (0.7: `write`/`edit`/
  `apply_patch`). A tool's `call_grading` name is what grants are recorded
  under. A resolver's reasoned `Ask` shows its text and `CardReason` on the
  card, a timed-out reasoned `rhai` launch is refused, an unattended
  collapse of it names the reason, and a mode grant on the call's `(tool,
  argument)` answers it like any `Ask` — unless the resolver named its own
  grant key (`AskReason::grant`): then only a grant on that key answers
  it, and an approval wider than `Once` records that key instead.
- **Changed behaviour (`skutter`):** an out-of-root call asks through
  `host::EscapeResolver`, the outermost resolver wrapper. Its approval is a
  mode-scoped grant on the ask's own key — `<tool>#outside-root` on the
  escaped path, for `bash` on `[workdir, command]` — so an in-root grant on
  a command never runs it elsewhere and an out-of-root approval never
  grants the bare command; not a mode-independent `(tool, path)` entry
  in `extra-roots.yml`, and a `Once` approval is no longer bound to its
  request id: the tools run whatever path the check cleared, so a symlink
  swapped between the check and the run is not caught (#449's guard is
  gone). Unattended, an escape follows the `Ask` ladder (the first collapses
  to a refusal, a repeat parks) instead of always parking. `extra-roots.yml`
  now holds `/cd --always` grants; entries earlier releases wrote are still
  honoured. `glob`/`grep` still never ask and drop an out-of-root match
  unless a durable `read` entry in that store covers it — the scratch dir
  or a `read` grant an earlier release persisted; a `read` approved now is
  a mode grant, which they do not ride (ADR-0132). `post_tool_use` runs
  inside `skutter`'s own tools, before their result returns, a script's
  calls included; an MCP tool's still fires from the event stream.
- **Left for #932:** `Mode::sandbox` and `Mode::sandbox_network` stay on
  the library's `Mode`, though only `skutter` (`host::sandbox`, `inspect`)
  reads them.
- **Instead:** register your own tools (the library ships none); confine
  them in the tool or in a `PermissionResolver` wrapper that returns
  `Grade::ask_keyed(..)`, as `skutter`'s `EscapeResolver` does; pass
  `.root(path)` for root-relative grading; implement `script_unbounded`,
  `call_grading` and a `Write` + `ArgKind::Path` declaration where your
  tool needs the clock exemption, a base tool's grade or the write floor.
  `skutter`: `FileExtraRoots` is folded into `state::ExtraRootStore`
  (`managed`/`at`), and `host_tools_with_extra_roots` is
  `HostToolOptions::extra_roots`; `hooks::CallPaths` is gone and `hooks::spawn` takes
  `(holly, hooks)` — call `hooks::wrap_host_tools` on the registry first.
