# entanglement

Headless, Rust-based AI coding agent **engine**. The reasoning + tool-execution
loop is decoupled from any UI and exposed as an async **actor**: a typed inbox of
`InMsg` and a broadcast outbox of `OutEvent`. Every interface is a thin adapter
over those two methods.

- Architecture & interfaces: [`docs/architecture.md`](docs/architecture.md)

## Status

Actor core + stdio head + TUI + local WebSocket `serve` head, with real LLM
backends wired (`entanglement-provider`: z.ai/OpenAI/Ollama + Anthropic +
Gemini). All
four interfaces now ship. The three-layer re-architecture (core / provider /
runtime) has landed — see
[`docs/adr/0006`](docs/adr/0006-core-dependency-hygiene-gate.md) and the crate
table below.

## The contract (one set of types, every head)

```
InMsg    : Prompt | Approve | Reject | ToolResult | AnswerQuestion | RetractQuestion | ReplaceQuestion | Stop
          | PauseSession | ResumeSession
          | SetMode | SetModel | SetGeneration | SetSessionMeta | SetToolOverlay | Oneshot | Spawn
          | ListSessions | ListQuestions | ListOperations | ListPending | ReplayFrom | CloseSession
          | McpList | McpAdd | McpRemove | McpAuth
          | RevertFiles | ForkSession | SetWorkdir | HibernateSession (trusted-only) | Resume (internal, not serialized) (harness → engine)
OutEvent : SessionStarted | SessionEnded | SessionHibernated | SessionList | QuestionList | OperationList | PendingList | History | Status
          | AgentChanged | ModeChanged | ModelChanged | GenerationChanged | SessionMetaChanged | ToolOverlayChanged | WorkdirChanged
          | McpList | McpChanged | McpAuthChanged | Throttle | PersistenceDegraded | PersistenceRecovered
          | Plan | TextDelta | ReasoningDelta
          | ToolCallDelta | ToolCall | ToolRequest | ToolExec | UserQuestion
          | ToolOutput | TaskList | Usage | Error | Done | Compacted | FileChange | PlanChanged
          | FilesReverted | SessionForked | SkillActive | AmbiguousRetry | SearchResult | ReasoningBlock (engine → harness)
```

Every frame is **session-scoped** (one connection multiplexes many sessions via
`SessionId`) and content frames carry a monotonic `seq` for dedup/ordering.

## Four interfaces, one ABI

| Head | Status | What it is |
| --- | --- | --- |
| **ABI (direct)** | ✅ | Hold a `Holly`, call `holly.send(InMsg)` / `holly.subscribe()`. Zero serialization. The foundation. |
| **stdio** (`skutter run` / `skutter pipe`) | ✅ | NDJSON over stdin/stdout — one-shot `run` (text or `--format json`, à la `opencode run`) and bidirectional `pipe`. `skutter sessions` lists past sessions; `skutter inspect prompt --agent <name> [--parts]` prints an agent's assembled system prompt (no engine); `skutter inspect agents [name]` shows the resolved agent registry with layer provenance — a table (name, model, default mode, layer, source) or one agent's full resolved identity + what lower layers it overrode; `skutter inspect modes [name]` lists the four permission modes or one mode's resolved rules; `skutter inspect skills [name] [--disclosures]` does the same for skills — a table (name, layer, root_dir, description), the exact `list_skills` roster the model gets (`--disclosures`), or a dry-run of the `load_skill` path substitution for one skill; `skutter inspect commands [name]` lists the user slash commands. |
| **TUI** (`skutter tui`) | ✅ | opencode-style terminal UI streaming `OutEvent`, tool-approval prompts, plan/task panels; an always-visible top status line shows where it works (`repo › path  ⎇ branch [worktree]`, configurable); `dark`/`light` themes with per-slot colour overrides (`NO_COLOR` honoured) and remappable keys (`Ctrl+X` leader by default; `Ctrl+X ?` lists the bindings in effect). |
| **WebSocket** (`skutter serve --port <N>`) | ✅ | axum `/ws` (+ `/healthz`), one `broadcast` fan-out per socket relayed as JSON text frames, inbound frames routed through the untrusted `send_from_wire` path, multiplexed by `SessionId`. **Local, single-user, loopback-bound** (`127.0.0.1` only) — the WS is a general protocol interface (a raw local script is as valid a client as any future client), so the `--allow-origin` check is opt-in, never mandatory ([ADR-0048](docs/adr/0048-serve-head-local-trust-model.md), #153). |

Building your own head — a multi-tenant server embedding the engine as a
library, rather than one of the four above — is covered in
[`docs/embedding.md`](docs/embedding.md): session-per-tenant namespacing, the
`send`/`send_from_wire` trust split, pluggable persistence and tool-execution
policy, dynamic per-session tool/prompt resolvers, hibernate/resume, and
approval-across-restart semantics, backed by three compiling examples —
[`embedded.rs`](entanglement-runtime/examples/embedded.rs),
[`embedded_lifecycle.rs`](entanglement-runtime/examples/embedded_lifecycle.rs),
and [`mcp_http.rs`](entanglement-runtime/examples/mcp_http.rs).

## Agents and permission modes

A session is a pair **(agent, mode)** — two independent axes
([ADR-0207](docs/adr/0207-permission-modes-replace-agent-borne-authority.md)).

An **agent** is *identity only* — `{ name, description, system_prompt,
model?, provider?, default_mode? }` (opencode-style `.md` files; frontmatter may also set
`include_brief`/`skills`, baked into the prompt at load time, and an optional
`default_mode` starting hint — below). It is chosen
once — `--agent`, `config.yml` `agent:`, or the `agent` tool's argument for a
spawned child — and is fixed for the session's life; there is no live agent
switch. Built-ins: `general` (the default coding agent and default spawn
target), `plan`, and `debug`. Any agent may be a session root or a spawn
target.

A **permission mode** carries every permission fact: `research` | `plan` |
`build` | `auto`, compiled into the runtime (tunable, add-only, via
`config.yml` `modes:`; `skutter inspect modes` shows the resolved rules).
Each mode grades a call `Allow`/`Ask`/`Deny` from its rule table — keys are a
tool name, `*`, argument-scoped `tool(pattern)`, workdir-scoped
`tool{pattern}`, or a capability class (`read`/`write`/`exec`/`plan`). A
rule naming the tool outranks a capability-class rule; a `*` rule competes
with a named rule by key length and with a class rule by strictness (the
stricter wins); within each kind the longest key wins; ties go to the
stricter grade. The `permissions:` ceiling grades the same way:

- `research` — read-only Q&A: `read` allowed, `write` and `plan` denied, a
  curated read-only shell set allowed, everything else asks.
- `plan` — like `research`, but `propose_plan` is allowed and only the
  `write` tool may create `.entanglement/plans/*.md`; `edit` and every other
  write is denied (ADR-0142/ADR-0145).
- `build` (the default) — `read`/`write`/`exec` allowed outright, bar a short
  destructive-command deny list (`rm -rf /`, `git push --force`,
  `git reset --hard`, …) and a list that asks (`git push`,
  `gh pr create`, `cargo publish`, …, and launching a `rhai` script).
- `auto` — the unattended posture: everything runs except what its deny
  list refuses outright (publishing and destructive commands); deletions
  (`gh … delete`, `cargo yank`, recursive `rm`, …), a path outside the
  project root, a credentialed `http()` host and writes of trusted scripts
  still ask, bounded by a 60s timeout
  ([ADR-0228](docs/adr/0228-rhai-path-and-args-skill-tools-frontmatter-removed.md),
  amending [ADR-0209](docs/adr/0209-auto-mode-defaults-to-prompt-not-deny.md)).

Select a mode with `--mode` on `run`/`tui` (or `config.yml` `mode:`) and
switch it live with `/mode` in the TUI. With neither set, a fresh session
starts in its agent's `default_mode` (the built-in `plan` agent starts in
`plan`; a hint is never wider than `build`, so never `auto`), else `build`; a spawned child only ever takes a `default_mode` no
wider than its parent's mode, and a resumed session keeps its replayed mode
([ADR-0218](docs/adr/0218-agent-default-mode-is-a-starting-hint.md)); a blocked model may ask to widen via
`request_mode`. Spawning is bounded by the mode's `max_depth`/`max_agents`.
Permission resolution and approval live entirely in the runtime.

**Undo / rewind** (ADR-0212): `write`/`edit`/`apply_patch` back up each
file's prior content in the session store, so in the TUI `/undo` reverts the
latest turn's file edits and `/rewind <N>` everything after turn `N` (bare
`/rewind` lists the turns that have edits, with file counts). A file you
changed since the agent did is never overwritten — it is reported as skipped.
Changes made by `bash`, `call` or `rhai` commands are **not** tracked. Other
heads send the trusted-only `InMsg::RevertFiles` and get `FilesReverted` back.

**Fork** (ADR-0223): `/fork` copies the active session into a new one with the
same history, model, mode and tool overlay, and moves you into it; `/fork <N>`
keeps only turns `1..N`. The original keeps running untouched. A fork starts
with an empty `/undo` history, since file backups belong to the session that
made the edit. Other trusted heads send `InMsg::ForkSession` and get the fork's
`SessionForked` back.

**Working directory** (ADR-0224): `/cd <dir>` moves the active session —
`bash`/`call`/`rhai` commands run there and relative paths in file-tool
arguments resolve against it; bare `/cd` goes back to the project root. A
directory outside the root needs your approval once: `/cd --session <dir>`
(this run) or `/cd --always <dir>`. Sub-agents spawned afterwards, compaction
successors and `/fork`s inherit it; the status line shows `⚠ cd <dir>` while
it differs from where the TUI runs. Other trusted heads send
`InMsg::SetWorkdir` (or `Spawn.workdir`) and get `WorkdirChanged` back.

Every session is advertised **one tool surface**, identical on every wire
and pinned for the session so the provider's prompt cache holds (ADR-0226):
the compiled-in `KERNEL` — the file tools `read`/`write`/`edit`/`apply_patch`,
`bash`, `web_fetch`, `rhai`, the orchestration tools (`agent`/`agent_send`,
`poll`, `ask_user`, `update_tasks`, `propose_plan`, `request_mode`), six
discovery tools — `list_agents`, `list_models`, `list_skills`, `load_skill`,
`list_mcp`, `mcp_tools` — and the `mcp(server, tool, args)` envelope, the only
way to call an MCP tool. MCP schemas are never advertised: `mcp_tools` shows a
server's tools with their schemas, and an `mcp(...)` call's args are checked
against the real schema before it reaches the server. Every configured or
provider-bundled MCP server is available unless `disabled: true`; nothing
connects at startup — the first `mcp_tools`/`mcp(...)` on a server connects it,
and every call is graded by the session's permission mode.

A plan is a **file** (`.entanglement/plans/<id>.md`), not an in-memory
snapshot: `propose_plan(content | path)`, allowed only in `plan` mode,
materializes or binds one, force-parks for approval, and — on approve —
switches the session's mode in place to the approver's pick (`build` or
`auto`; a bare accept means `auto`) so the same turn continues into implementation
(#513, ADR-0145; ADR-0207 §7). `OutEvent::TaskList` (markdown `content`) is a
separate, simpler snapshot from `update_tasks`: permission-gated but carrying
no host resource, so the runtime intercepts it out of the tool registry and
emits the snapshot instead of dispatching (#231, ADR-0049). Every head
renders both natively.

**Definitions are data, layered** (embedded < user < project, later wins).
Agents (`ENTANGLEMENT_AGENTS_DIR`) and skills (`ENTANGLEMENT_SKILLS_DIR`) are
`.md` files with YAML frontmatter — drop one in and it joins the registry with
no code change; `skutter inspect agents|skills` shows the resolved set with
layer provenance. **User slash commands** (`ENTANGLEMENT_COMMANDS_DIR`, #733,
[ADR-0221](docs/adr/0221-user-slash-commands-are-head-side-prompt-templates.md)) are prompt templates: drop
`.entanglement/commands/review.md` (or reuse `.claude/commands/` /
`.opencode/command/` files; `frontend/review.md` is `/frontend:review`) and
`/review src/a.rs` in the TUI or `skutter run "/review src/a.rs"` sends its
body with `$ARGUMENTS` / `$1`..`$9` filled in. Frontmatter `description` and
`argument-hint` show in the slash popup; `agent`, `model` and `allowed-tools`
are ignored, and `` !`cmd` `` / `@file` are sent literally — a command is
plain text, never shell or file access. Built-in TUI commands win on a name
clash; `skutter inspect commands` lists what resolved. Beyond the root-contained file tools, `bash` (a real shell) is always registered
([ADR-0195](docs/adr/0195-bash-is-the-default-exec-and-curated-read-only-rules.md))
and graded by the session's mode like everything else. `glob`, `grep` and
`call` (argv exec, no shell) are registered but never advertised: they are
script-only `rhai` bindings (`call` as `exec`), each call graded under its own
name. The sandboxed `rhai`
scripting tool ships in every default build (droppable via
`--no-default-features`, #502) — every call a script makes goes through the
same permission path as a direct one, MCP tools are callable as
`mcp::<server>::<tool>(…)`, a skill can ship a `lib.rhai`, and `http()` can
attach credentials the user maps per host in `config.yml` `http_auth:` (never
visible to the model; the first use per host is always approved,
[ADR-0227](docs/adr/0227-one-binding-path-namespaces-http-skill-libraries.md)). `web_fetch` reads a public http(s) page as
Markdown behind an SSRF guard: private, loopback, link-local and metadata
addresses are refused, DNS answers are pinned, and every redirect is checked
again. It is graded like `curl` (allowed in `build` and `auto`, prompted in `research`/`plan`) and
can be dropped with the `web-fetch` feature
([ADR-0220](docs/adr/0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md)).
In the TUI, `Ctrl+V` pastes a clipboard image into the prompt when the model's
catalog entry sets `supports_vision`.

## Crates

Three crates, two seams (core ↔ provider, core ↔ runtime). Dependency direction
is `provider (leaf) ← core ← runtime` ([ADR-0053](docs/adr/0053-invert-core-provider-seam.md)).

| Crate | Role | Hard rule |
| --- | --- | --- |
| `entanglement-provider` | **leaf** crate owning the LLM ABI: the `Llm` **trait** + DTOs (`LlmRequest`/`Event`/`Stream`, `LlmFactory`, `ToolCall`/`ToolSpec`) + wire `Message`; z.ai/OpenAI/Ollama + Anthropic + native Gemini clients (#309); connection pool, retry, rate-limit, reasoning stream. Usable **standalone** for raw LLM queries. | no `entanglement-*` deps; owns `reqwest`. |
| `entanglement-core` | actor engine: `Holly`, `InMsg`/`OutEvent`, agent turn loop, `Context`. Advertises tool *schemas* (`ToolSpec`) only — holds no executable tools. Depends on provider, drives `dyn Llm`, re-exports the ABI. | **No UI/web-server deps** (`clap`/`axum`/`crossterm`/`ratatui` forbidden); `reqwest` is transitive via provider (ADR-0053). Enforced via `make tree`. |
| `entanglement-runtime` | the head crate (binary `skutter`): the `Tool` **trait** + `ToolRegistry` (moved from core, ADR-0059), host-tool impls, tool execution + permission dispatch (✅ #58/#59/#206), approval, user sessions, all transports (stdio ✅, TUI ✅, WS ✅ #153). Selects the concrete provider + glues it to core. Feature-gated `cli`/`provider`/`tui`/`serve`/`mcp-http`/`rhai`/`web-fetch` (`default = ["tui", "serve", "mcp-http", "rhai", "web-fetch"]`, #502, #736); `--no-default-features` is a lean embeddable library (ADR-0025). | `--no-default-features` stays CLI/TUI/transport-free; `make check-lean` enforces. |

## Install

```bash
cargo install entanglement-runtime   # installs the `skutter` binary
```

## Set a provider key

Provider API keys live in a managed env file
(`${config_dir}/entanglement/.env`, override `ENTANGLEMENT_ENV_FILE`), loaded at
startup for any var the real environment left unset (env > file). Set one without
hand-editing the file (#304, [ADR-0073](docs/adr/0073-managed-env-file-writer-and-key-surfaces.md)):

```bash
skutter config set-key zai                 # hidden prompt (never echoed)
skutter config set-key openai --key sk-…   # or pass it directly
echo "sk-…" | skutter config set-key zai   # or pipe it (scripting/CI)
```

z.ai has two catalog entries sharing `ZAI_API_KEY`, one per key type: `zai`
(the GLM Coding Plan endpoint, auto-detected) and `zai_paas` (the
pay-as-you-go endpoint). A pay-as-you-go key is rejected by the Coding Plan
URL — select it with `ENTANGLEMENT_PROVIDER=zai_paas` (or `provider: zai_paas`
in `config.yml`).

Inside the TUI, `/key` opens the same dialog (provider list → masked input); a key
set there is picked up on the next `/model` switch with no restart.

**First run, no provider configured** ([ADR-0222](docs/adr/0222-no-silent-echo-first-run-onboarding-and-init.md)):
bare `skutter` opens a setup dialog before the TUI starts — pick a provider
(keyless `ollama` asks for its base URL instead of a key), type the key
(masked, saved to the managed `.env`), pick a model (the catalog default is
preselected); the choice is saved as `provider:`/`model:` in `config.yml`.
`Esc` continues in a clearly labeled echo mode for that run. `run`, `pipe` and
`serve` never fall back silently: they print which key variables to set (or
`skutter config set-key <provider>`) and exit 2. The `EchoLlm` debug stub is
explicit-only — `ENTANGLEMENT_PROVIDER=echo`.

Once a provider is set, `/init` in the TUI asks the model to write an
`AGENTS.md` brief for the repo (a normal turn, so the write goes through the
session's permission mode); with an `AGENTS.md` already present it refuses
unless you pass `/init --update`, which improves the existing file.

## Configure

Settings live in a layered `config.yml` (embedded < `${config_dir}/entanglement/`
< `<repo>/.entanglement/`); `skutter inspect config` prints every resolved value
with the layer that won it, plus the provider catalog's advanced wire facts.
TUI preferences share one live-reloaded block:

```yaml
tui:
  notify: true   # OSC 9 desktop notification on top of the attention bell
  mouse: false   # give the terminal back its native text selection
  statusline:    # the top row: where the TUI is working
    template: "{repo} › {cwd_rel}  ⎇ {branch} {worktree}"   # left-aligned
    right: "{session}"                                      # right-aligned
    # command: jq -r '"\(.repo // .cwd) ⎇ \(.branch // "-")"'  # replaces `template`
    # timeout_ms: 1000
  theme:         # `dark` (default) | `light` | a base plus slot overrides
    base: light
    slots: { status.branch: cyan, diff.add: "#2e7d32", muted: 244 }
  keybindings:   # action -> key(s); replaces that action's defaults
    leader: ctrl+b
    pick_model: [leader m, ctrl+o]
    newline: [shift+enter, alt+enter]
```

**Status command.** `statusline.command` swaps the left template for the
first line a shell command prints (run with `sh -c`, at most once a second,
killed after `timeout_ms`). It gets a JSON payload on stdin — `{"v": 1,
"session", "cwd", "root", "repo", "branch", "worktree", "agent", "mode",
"model": {"id", "name"}, "cost_usd", "context_pct", "action", "tasks":
{"done", "total"}, "workdir"}`, unknown facts `null` — and its colours are stripped (the
theme colours the line). On a failure or timeout the template line comes
back, with one toast until the command succeeds again. It inherits the
TUI's environment minus provider API keys, and a project-layer
`config.yml` may set it (the repo is trusted, like its hooks).

**Themes.** Every colour the TUI draws is a named slot (`message.bg`,
`accent`, `success`, `warning`, `error`, `muted`, `selection.bg`, `approval`,
`diff.add`/`diff.remove`/`diff.context`, `status.*`, … — the full list is in
the scaffolded `config.yml`); a colour is a named ANSI colour, `#rrggbb` or a
256-colour index. An unknown slot or bad colour warns once and keeps the
base's colour. `light` keeps every foreground at WCAG ≥ 4.5:1 on its
backgrounds; code blocks follow the base (`base16-ocean` dark/light).
`NO_COLOR` (set, non-empty) turns all colour off — bold/dim/reverse still
mark emphasis and selection.

**Keys.** Actions: `leader submit newline cycle_mode cycle_mode_back paste_image
scroll_up scroll_down toggle_reasoning jump_attention new_session
list_sessions pause_toggle pick_agent pick_model inspect toggle_sidebar
open_editor export command_palette interrupt quit help`. A key is
`[ctrl+][alt+][shift+]<key>` (a character or `enter`, `tab`, `esc`,
`pageup`, `space`, `f5`, …) or `leader <key>`; `[]` unbinds. `Ctrl+C`
(interrupt / two-stage quit), `Ctrl+Q` (quit) and `Esc` are fixed, and a bare
printable key can't be bound directly (nor a dialog navigation key to the
`jump_attention`/`list_sessions` actions, which fire over open dialogs). Two actions on one key in the same
context (direct or after the leader), an unknown action or a bad key rejects
the whole remap with one warning naming the culprits, and the previous map
stays. Both blocks live-reload; the help dialog (`Ctrl+X ?`) shows the keys in
effect.

Status-line placeholders: `{repo} {cwd} {cwd_rel} {branch} {worktree}
{session} {agent} {mode} {model} {cost} {context_pct} {action} {tasks}
{workdir}` (`{workdir}`: the session's `/cd` directory, empty at the root). An
empty value disappears with the separator in front of it; an unknown `{name}`
renders literally and is reported once by `skutter inspect config`. Git facts
are read from `.git` directly (no `git` process), so a detached `HEAD` shows a
short sha and an unreadable one `?`.

**Undo backups.** `/undo` and `/rewind` restore files from per-write
backups (`write`/`edit`/`apply_patch` only — never `bash`). Their caps and
an off switch live in one block, picked up live for the next write:

```yaml
undo:
  enabled: true          # false: record nothing new; existing backups still restore
  max_file_bytes: 8MiB   # a bigger prior version is skipped (undo says so)
  max_root_bytes: 256MiB # stored budget per root session
```

A size is plain bytes or `<n><unit>` — `B`, `KB`/`MB`/`GB` (×1000),
`KiB`/`MiB`/`GiB` (×1024) — between 1 byte and 64 GiB; anything else is a
config error.

The few remaining environment knobs beyond provider keys/models:
`ENTANGLEMENT_STATE_DIR` relocates every file skutter manages for you (grants,
model/generation/aux pins, escape-root grants, MCP/LLM OAuth tokens, the
last-seen MCP tool-name cache) in one go,
and `ENTANGLEMENT_DEBUG=bodies,echo,no-shared-state` switches on developer
diagnostics. Knobs retired in 0.7 still print one startup warning naming
their replacement ([ADR-0213](docs/adr/0213-prune-env-vars-and-catalog-knobs.md)).

## Build & develop

Requires stable Rust (pinned via `rust-toolchain.toml`). Build jobs capped at 4
in `.cargo/config.toml`.

```bash
make run          # one dummy turn, text output
make run-json     # one dummy turn, NDJSON events
make run-tui      # launch the terminal UI
make pipe         # stdio pipe head — InMsg NDJSON on stdin, OutEvent NDJSON on stdout
make serve        # local WebSocket head on 127.0.0.1 (ARGS='--port 4517')
make test         # unit + integration (HOME/XDG sandboxed — fails if a test writes there)
make test-live    # opt-in live z.ai probes (needs ZAI_API_KEY)
make test-live-matrix # opt-in live z.ai tool-use matrix across GLM models (needs ZAI_API_KEY)
make lint         # clippy --all-targets -D warnings
make verify       # check-fmt + tree + check-lean + file-cap + userid + lint + test (CI-equivalent)
make tree         # cargo tree -p entanglement-core (UI/web-server dep hygiene gate)
make check-lean   # runtime --no-default-features stays CLI/TUI-free (ADR-0025 + ADR-0053)
make msrv         # cargo check (full + lean runtime) on the declared MSRV (rust-version)
make coverage     # cargo llvm-cov --workspace, fails under COV_MIN% (release gate)
make install      # cargo install --path entanglement-runtime → `skutter` in $CARGO_HOME/bin
make build | check | fmt | clean
```

Drive commands through `make`, not raw `cargo`.

## CI

GitHub Actions drives the same `make` targets ([`.github/workflows/`](.github/workflows/)):

- **`ci.yml`** — runs `make verify` on every PR and every push to `master`,
  putting the `tree` (ADR-0006) and `check-lean` (ADR-0025) hygiene gates under
  automation, and a separate `make msrv` job that typechecks on the workspace
  `rust-version` toolchain (#728) so the declared MSRV is tested, not asserted.
- **`release.yml`** — on a `v*` tag, runs `make verify` **plus** `make coverage`
  (`cargo llvm-cov --workspace`, fails under `COV_MIN`%) and uploads the
  lcov/Cobertura reports as an artifact, so a release is blocked on green tests.

`make coverage` needs `cargo-llvm-cov` locally: `cargo install cargo-llvm-cov
--locked`.

## License

MIT — see [LICENSE](LICENSE).
