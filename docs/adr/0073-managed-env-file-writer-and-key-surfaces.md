# 0073. One shared writer for the managed env file, with a CLI and TUI surface

- Status: Accepted
- Date: 2026-07-15
- Builds on the managed provider-key env file of [0047](0047-local-trust-boundary.md)
  (layered definitions, `#220`) and the realtime model/provider switch of
  [0063](0063-realtime-model-provider-switch.md). Issue #304 (part of #302).

## Context

The managed env file (`${config_dir}/entanglement/.env`, override
`ENTANGLEMENT_ENV_FILE`, #220) was **scaffold-and-read only**:
`scaffold_if_missing` drops commented `#KEY=` placeholders on first run, and
`load()` reads `KEY=VALUE` lines into the process env for any var the real
environment left unset (env > file). There was no writer — setting a key meant
hand-editing the file, which is a poor first-run experience and impossible from
inside the TUI.

We want two surfaces — a `skutter config set-key <provider>` command and a TUI
`/key` dialog — without duplicating the file-rewriting logic or the subtle
"which line do I replace?" rules that must stay consistent with how `load()`
reads the file back.

## Decision

**One shared writer** in `entanglement-runtime::config::env_key`, both surfaces
drive it.

- **Pure `upsert(text, key, value) -> String`.** Replace the first *live* `KEY=`
  line (first-occurrence-wins, matching `load()`'s no-override read); else
  replace the first commented `#KEY=` / `# KEY=` placeholder in place; else
  append. Every untouched line — including its exact terminator — is preserved
  byte-for-byte. Idempotent: applying it twice yields the same text.
- **`set_key(key_name, value) -> Result<PathBuf>`.** Resolves the managed path
  (a loud error when there is none — no config dir and no `ENTANGLEMENT_ENV_FILE`),
  creates the file from the commented `template` when absent, and writes
  **atomically**: a sibling temp file in the same directory, tightened to
  `0o600` on unix, then renamed over the target. Empty or `\n`-containing values
  are refused — they cannot be represented in the single-line `KEY=VALUE` format.

**CLI** (`config::keys`, behind `cli`+`provider`): `skutter config set-key
<provider> [--key V]` looks the provider up in the catalog, resolves its
`key_env` (a keyless provider like Ollama is a clean error), and sources the
value from `--key`, a hidden `rpassword` prompt, or a plain stdin read when stdin
is piped. The value is never echoed; a warning fires when the process env already
carries a *different* value for that var (env > file, so the file write won't
take effect until the env var is unset). Dispatched in the pre-engine fast path,
like `inspect`/`sessions`.

**TUI `/key` dialog** (`tui::key_dialog`): a two-stage modal after the `/model`
picker pattern — provider list → masked input (`masked()` renders bullets only,
the key is never shown). On submit it calls `set_key(...)` then
`std::env::set_var(...)` so the live model resolver ([0063](0063-realtime-model-provider-switch.md))
binds the new key on the next `/model` switch — **no restart** (startup
auto-detect still needs one). A status line (never the key) is recorded into the
transcript; `Esc` wipes the buffer.

## Consequences

- Both surfaces share one place where the "first live line, else placeholder,
  else append" rule lives, and that rule is pinned to `load()`'s semantics by
  construction and by a unit-test matrix (placeholder/live/append/duplicates/
  idempotent/byte-preserving).
- `env_key` is pure std + `anyhow` — it stays in the lean library and out of the
  hygiene gates. Only `keys` (rpassword + catalog) is feature-gated, so
  `make check-lean` stays green.
- A key set via the TUI is live for switching within the session but not for the
  startup auto-detect; the status line says so implicitly by naming only the
  var + path.
- The atomic temp-file-in-dir + rename keeps a concurrent reader from ever seeing
  a half-written credentials file, and `0o600` keeps it user-only.

## Rejected alternatives

- **Write from each surface directly.** Duplicates the line-matching rules and
  risks the CLI and TUI diverging from `load()`.
- **A dotenv library.** Overkill for a handful of `KEY=VALUE` lines, and none
  preserve comments/placeholders/ordering byte-for-byte the way the scaffold
  needs.
- **Non-atomic in-place rewrite.** A crash mid-write would corrupt the file that
  holds every provider credential.
- **Send `SetModel` on TUI submit to apply the key immediately.** The key only
  matters at the *next* resolve; priming the process env is enough, and the user
  drives `/model` when they want to switch. Forcing a re-resolve would rebind the
  LLM as a side effect of setting a key.
