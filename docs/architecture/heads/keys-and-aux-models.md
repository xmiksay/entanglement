# Heads & persistence §6c–§6d — Managed key file, onboarding & auxiliary models

> Part of the [heads & persistence module](../heads-and-persistence.md) of the [architecture overview](../../architecture.md).

## 6c. Managed provider-key env file — [ADR-0073](../../adr/0073-managed-env-file-writer-and-key-surfaces.md) (`config::env_file` + `config::env_key`)

Provider API keys live in `${config_dir}/entanglement/.env` (override
`ENTANGLEMENT_ENV_FILE`, #220), a sibling of `config.yml`. `env_file` scaffolds a
commented `#KEY=` template on first run and `load()`s `KEY=VALUE` lines into the
process env for any var the real environment left unset (env > file). `env_key`
(✅ #304) is the **writer** both key surfaces ([TUI controls](tui-controls.md)) share: a pure `upsert(text,
key, value)` (replace the first *live* `KEY=` line — first-occurrence-wins,
matching `load()` — else the `#KEY=`/`# KEY=` placeholder, else append; other
lines byte-for-byte preserved; idempotent) plus `set_key(key, value) ->
Result<PathBuf>` (loud error with no managed path; create from `template` when
missing; atomic temp-file-in-dir + rename; `0o600` on unix; reject empty/`\n`
values). `env_key` is pure std + `anyhow` (lean/gate-clean); only the `keys`
handler (rpassword + catalog) is feature-gated behind `cli`+`provider`.
`set_key_at(path, …)` is the same write against an explicit path.

**First-run onboarding** (✅ #732,
[ADR-0222](../../adr/0222-no-silent-echo-first-run-onboarding-and-init.md),
`tui::onboarding` + `provider_setup`): `main` checks
`provider_setup::provider_configured` (an `ENTANGLEMENT_PROVIDER`, a config
`provider:`, or a non-empty catalog `key_env` after the `.env` load) before
`boot`. Unconfigured, the TUI head runs a standalone raw-mode dialog
(`onboarding::state` is the pure key-driven state machine, `draw` the
render): provider (catalog order, OAuth-only entries excluded, keyless ones
included) → masked key, or a base URL prefilled from the catalog for a keyless
provider → model (catalog default preselected). `persist` writes the key (or an
edited `<NAME>_API_BASE`) through `env_key::set_key_at` and `provider:`/`model:`
through `write_key::save_key_at`; each failure is a stderr warning without the
key. `apply` then primes the process env (as `/key` does) and the in-memory
config, so `boot`'s `select_provider` simply binds the choice. `Esc` pins the
in-memory provider to `echo` for this run only; `App::set_active_provider("echo")`
records a "no provider configured — echo mode" transcript notice and the echo
model is displayed as `echo mode (no provider)`. `Ctrl+C` quits. The other
heads never onboard: with nothing configured, `select_provider` prints the hint
and exits 2.

## 6d. Per-purpose auxiliary models & auto session titles — [ADR-0154](../../adr/0154-per-purpose-auxiliary-models.md)/[ADR-0183](../../adr/0183-narrate-purpose-and-per-user-aux-pins.md) (`aux_llm` + `config::aux_models` + `session_title` + `narrate`)

Side transformations (a compaction summary, an auto session title) can run on
a cheaper/faster model than the session's own. The pin store is a managed
`<state dir>/aux-models.yml` (`ENTANGLEMENT_STATE_DIR`, [ADR-0213](../../adr/0213-prune-env-vars-and-catalog-knobs.md), sibling
of `agent-models.yml`, same
`with_locked_file`+`atomic_write` discipline) mapping a closed `Purpose` enum
(`summarize` | `session_title` | `narrate`) to a `{provider, model}` pin.
`aux_llm::AuxLlmRegistry` resolves a `Purpose` → fresh `Box<dyn Llm>` by
reusing the catalog `ModelResolver` the runtime already builds at startup (the
same closure `SetModel` calls, capturing the catalog + the warm per-endpoint
client, so an aux client rides the shared pool — and a second provider is just
a second endpoint pool/key, already supported), **falling back to the primary
model's `LlmFactory`** when a purpose is unset or its pin no longer resolves
(unknown to the catalog, missing key — logged at debug, never wedging the
transformation). Three consumers reach it by deliberately different routes:

- **The session-title generator** (`session_title.rs`, behind the `provider`
  feature): a background task off `holly.subscribe_inbound()` that, on the
  **first `Prompt` of an unnamed session**, spawns a detached one-shot call to
  the `session_title` aux LLM (prompt capped ~2k chars, output capped 80) and
  sets the result via an `InMsg::SetSessionMeta` with **`if_unset: true`**
  ([ADR-0151](../../adr/0151-settable-session-metadata.md)) — best-effort
  throughout (a failed/empty generation is logged and dropped), idempotent per
  session (a titled-set guards late second prompts), and never clobbering an
  already-named session (#553): `if_unset` makes the engine's fold a no-op
  once `Session.name` is already `Some(_)`, so a late generated title can
  never win a race against — or overwrite — a `/name` set (in either arrival
  order) or a name a resumed process's replay already restored. The generator
  also folds a `Resume`'s replayed `SessionMetaChanged` history (per session
  id, last-write-wins) off the same inbound fan-out to pre-seed its
  in-process titled-set, so an already-named resumed session skips the aux
  call on its next prompt too, not just the write. It has no session backend
  to fall back to, so it calls `AuxLlmRegistry::resolve` directly and an unset
  pin yields the primary model. It fires **concurrently** with the main turn
  by default — except when `AuxLlmRegistry::concurrency_cap(SessionTitle)`
  reports the resolved model's effective per-model cap
  ([ADR-0140](../../adr/0140-per-model-concurrency-cap-layered-on-endpoint-cap.md))
  at 1 or below ([ADR-0158](../../adr/0158-defer-session-title-aux-call-under-contended-primary-concurrency.md),
  #589): with only one permit, the main turn's own request is guaranteed to
  hold it first, so the generator instead waits for that session's first
  `Done`/`Error` (bounded, 300s safety net) before making its aux call,
  sequencing the two instead of racing for a permit neither can win early.
- **Session compaction** runs *inside core*, which reaches the pin through the
  `EngineConfig::aux_llm_resolver` seam built by `AuxLlmRegistry::resolver`
  (see the engine doc's *Auxiliary models* section): there `None` means "use
  the session's own backend" — strictly better than a fixed primary, since a
  live `/model` switch keeps applying to compaction.
- **The action narrator** (`narrate.rs`, #635/ADR-0181, behind the `provider`
  feature): a background task off `holly.subscribe()` that, on **every
  `OutEvent::ToolCall`** (display-only, emitted before execution for every
  tool call), spawns a tracked call to the `narrate` aux LLM (input capped
  ~500 chars, output capped 60) turning `tool(input)` into a short
  present-tense phrase, and sends it back via `InMsg::SetSessionMeta { action:
  Some(_), if_unset: false, .. }` — `action` is unconditionally overwritten on
  `Some` (unlike `name`'s `if_unset` guard), so it's the first in-tree
  producer of [ADR-0151](../../adr/0151-settable-session-metadata.md)'s
  mid-turn-mutable `Session.action`. Like the title generator, it has no
  session backend to fall back to, so it calls `AuxLlmRegistry::resolve`
  directly. Unlike the title generator (fires once per session, permanently
  idempotent), it bounds a burst of tool calls with an **at-most-one-in-flight-
  per-session** guard — a `ToolCall` landing while a narration call is already
  running for that session is skipped, not queued, and the guard clears when
  that call's task finishes, so the next tool call after that gets a fresh
  narration.

**Fail-fast aux traffic** (#560 follow-up, no ADR — robustness hardening
within ADR-0154's existing decision): every aux request (`narrate`,
`session_title`, and `summarize` when a pin actually resolved — not its
session-fallback case, which keeps the ordinary ladder) rides
`LlmRequest::retry = Some(RetryConfig::minimal())` — at most one retry, a
short backoff/response-header wait, and a tightly bounded 429 park — instead
of the endpoint's full 5-attempt LLM-tuned ladder, so a dead pinned endpoint
(the triggering incident: `aux-models.yml` pinning `narrate`/`session_title`
to an unreachable local Ollama) fails one probe fast rather than
retry-storming on every tool call. `narrate`/`session_title` additionally
gate through `AuxLlmRegistry::try_resolve`, a per-`(purpose, provider,
model)` cooldown (`AuxCooldown`, 60s, mirroring ADR-0201's
`recent_enable_failures` shape) that short-circuits a call entirely — no
client built, no network touched — while a recent failure's window is still
open; a `tracing::warn!` fires once per new window (`note_failure`'s return
value gates it), not once per suppressed call. `summarize`'s pinned case
relies on the retry override alone (no cooldown gate): compaction is rare
enough, and infrequent enough per session, that the storm risk doesn't apply
the way it does to `narrate` firing on every tool call — but the fail-fast
`RetryConfig` still keeps a dead `summarize` pin from stalling a `/compact`
or auto-compact for the ladder's worst case.

The TUI surface is `/aux-model <purpose> <provider>/<model>`
(`parse_aux_model_args` — the raw-text re-parse pattern; `title` is accepted
as an alias for `session_title`; bare `/aux-model` or `/aux-model list`
renders the current pins), writing the pin through the shared store handle so
the live registry sees it with no restart. Bare `/set`'s dialog has an Aux tab
over the same store; it can move a pin but not clear one (the store has no
remove), so "(primary model)" is offered only for a purpose that started
unpinned.

**Per-user aux pins** (#635/[ADR-0183](../../adr/0183-narrate-purpose-and-per-user-aux-pins.md),
conforming to [ADR-0181](../../adr/0181-userid-leaves-the-runtime-crate.md)):
the runtime holds **no per-user module** — a multi-user embedder builds a
per-user registry itself from two public pieces. It looks the user's pins up
in its own storage (it owns the session→user mapping) and wraps them via
`AuxModelStore::in_memory` (the production sibling of the test-only
`for_test`); and it closes its multi-user `ModelResolver` over the right user
in a small adapter closure *before* constructing an ordinary, unmodified
`AuxLlmRegistry` — the registry carries no user notion and always passes
`None` to its injected resolver, so the closure substitutes the captured
user. That wrap also fixes a real failure mode: `resolve`/`resolve_pin`
always call the resolver with a hardcoded `None` user, and a multi-user
`ModelResolver` ([ADR-0147](../../adr/0147-multi-user-mode-embedder-api.md))
treats a missing user as a hard error — unwrapped, every aux call would be
rejected regardless of the pins. A per-user registry is typically built once
at session start and cached by the embedder; `AuxLlmRegistry` re-reads its
wrapped `AuxModelStore` on every `resolve`/`resolve_pin` call. `skutter`'s
own heads keep the one process-global `AuxModelStore`/`AuxLlmRegistry`,
byte-identical.

---

Previous: [§6 — The TUI head: lifecycle controls, commands & editor](tui-controls.md) · Next: [§6b — Session persistence & resume](persistence.md)
