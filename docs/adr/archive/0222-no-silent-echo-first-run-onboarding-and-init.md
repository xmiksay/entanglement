# 0222. No silent echo: first-run onboarding and `/init`

- Status: Accepted
- Date: 2026-10-11
- Extends: [ADR-0073](0073-managed-env-file-writer-and-key-surfaces.md)
  (managed `.env` writer and key surfaces),
  [ADR-0221](0221-user-slash-commands-are-head-side-prompt-templates.md)
  (a command is head-side sugar for typing a prompt)

## Context

With no provider key, `skutter` used to fall back to `EchoLlm` and print one
stderr line. In the TUI that line is gone once the alternate screen opens, so
a new user typed a prompt and got an echo of their own request back, with no
hint why. `run`/`pipe`/`serve` did the same: a script "succeeded" with output
from a stub. Issue #732 asks for a first-run dialog instead, an actionable
hint for the non-TUI heads, and a `/init` command that writes an `AGENTS.md`
brief.

## Decision

1. **Echo is explicit-only.** `ENTANGLEMENT_PROVIDER=echo` (or `provider:
   echo` in `config.yml`) still selects the stub. Auto-detect that finds no
   key no longer falls back to it.

2. **Non-TUI heads exit.** `run` (explicit or implicit), `pipe` and `serve`
   with nothing configured print a multi-line hint to stderr and exit 2, the
   code every other provider-selection error uses. The hint lists the
   catalog's key variables, `skutter config set-key <keyed providers>`, the
   keyless `ENTANGLEMENT_PROVIDER=ollama`, and `ENTANGLEMENT_PROVIDER=echo`.
   When the catalog has OAuth entries, the hint also names them with
   `skutter config connect <p>` and says to set `provider:` afterwards.
   All three heads behave the same. A `pipe`/`serve` client that wants the
   stub asks for it, so it never mistakes stub output for model output.

3. **The TUI onboards before boot.** "Configured" means
   `ENTANGLEMENT_PROVIDER` is set (any value, so a bad one still reaches the
   loud selection error), or `config.yml` sets `provider:`, or some catalog
   `key_env` is non-empty after the managed `.env` load. When none of these
   holds, a small dialog runs before `boot`. Provider selection happens in
   `boot`, so asking first means the engine simply starts on the user's
   choice and nothing has to be rebound live. The steps:
   - Pick a provider. Every catalog entry is listed except OAuth-only ones,
     which need `skutter config connect`. Keyless entries are included.
   - Enter the key, masked, for a keyed provider. A keyless provider gets a
     base-URL prompt instead, prefilled from `<NAME>_API_BASE` when set,
     else from the catalog.
   - Pick a model. The catalog default is preselected.
   - Save. The key, or an edited base URL as `<NAME>_API_BASE`, goes through
     the existing managed `.env` writer (`env_key`), the same path `/key` and
     `config set-key` use. `provider:` and `model:` go to the user
     `config.yml` through the comment-preserving key writer, so the next start
     picks the same provider without asking. The process env and the
     in-memory config are then primed the way `/key` primes them.

   Each failed write becomes a stderr warning that never contains the key,
   and the session still starts on the in-memory choice.

   `Esc` on the first step continues in echo mode for this run only. Nothing
   is saved, so the next start asks again. The transcript shows a "no
   provider configured — echo mode" notice and the status line shows the
   model as `echo mode (no provider)`. `Ctrl+C` quits.

4. **`/init` is head-side sugar for a prompt.** It sends a built-in template
   plus a bounded repository snapshot as an ordinary `InMsg::Prompt`, the
   same way ADR-0221 handles user commands. The model writes `AGENTS.md` with
   its normal `write`/`edit` tools, so the session's permission mode grades
   the write like any other: `research` refuses it, and `build` grades it as
   usual. There is no bypass. The snapshot reuses the TUI's `@file` index,
   which is the `glob` walk with its scan and result caps and `.gitignore`
   pruning. If that index has not landed yet, the same bounded walk runs once.
   The snapshot lists notable files, top-level entries, a file-type
   histogram, and at most 200 paths, shallowest first. If `AGENTS.md` already
   exists, a bare `/init` refuses. `/init --update` is the confirmation: it
   asks the model to improve the existing file in place. Text after the flag
   is passed through as extra instructions. `init` joins the reserved
   built-in names, so a user command named `init` is dropped with a load
   warning.

## Consequences

- A user, script or CI job that relied on the silent fallback gets exit 2.
  The fix is one env var: `ENTANGLEMENT_PROVIDER=echo`. The integration
  harness (`isolated_skutter`) now sets it by default, which also stops a
  developer's real key from turning a test into a live call.
- Picking a provider in the dialog pins `provider:` and `model:` in the user
  config. A key for an earlier catalog provider added later will not displace
  that choice. That is intended: the user chose. Edit or remove the keys to
  change it.
- `skutter init` (a CLI twin of `/init`) is not added. `skutter run` with the
  same request covers the scripted case.

## Alternatives considered

- **An in-TUI dialog after boot, rebinding through `SetModel`.** This needs
  the engine up on echo first, followed by a live rebind of the bootstrap
  session, with more moving parts in the at-cap `App`. Asking before boot
  makes the choice an ordinary startup selection.
- **Keep echo for `pipe`/`serve`.** Their clients cannot see stderr hints any
  better than a script can. One rule for every non-TUI head is simpler.
- **Let `/init` write the file itself, head-side.** That would bypass the
  permission mode and turn the head into a file writer. A prompt keeps one
  write path.
