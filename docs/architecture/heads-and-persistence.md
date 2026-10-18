# entanglement Architecture — Heads & session persistence

> Part of the [architecture overview](../architecture.md). The *why* behind each choice is in the [decision log](../adr/README.md).

This module is split into the pages below, one per seam; each stays under the 400-line doc cap (`make doc-cap`). Every section heading the single-file version had is kept here, pointing at where that section now lives.

| page | covers |
| --- | --- |
| [stdio-and-serve](heads/stdio-and-serve.md) | §6 — Heads: ABI, stdio & WebSocket |
| [tui](heads/tui.md) | §6 — The TUI head |
| [tui-controls](heads/tui-controls.md) | §6 — The TUI head: lifecycle controls, commands & editor |
| [keys-and-aux-models](heads/keys-and-aux-models.md) | §6c–§6d — Managed key file, onboarding & auxiliary models |
| [persistence](heads/persistence.md) | §6b — Session persistence & resume |

## 6. Heads — ADRs [0005](../adr/0005-ndjson-stdio-head.md) (stdio), 0001 (ABI), [0010](../adr/0010-single-head-crate-and-bash-opt-in.md) (packaging), [0011](../adr/0011-tui-head-ratatui-crossterm.md)–[0015](../adr/0015-rich-text-pipeline-syntect.md) (TUI)

Now in [§6 — Heads: ABI, stdio & WebSocket](heads/stdio-and-serve.md#6-heads--adrs-0005-stdio-0001-abi-0010-packaging-00110015-tui).
Continued in [§6 — The TUI head](heads/tui.md).
Continued in [§6 — The TUI head: lifecycle controls, commands & editor](heads/tui-controls.md).

## 6c. Managed provider-key env file — [ADR-0073](../adr/0073-managed-env-file-writer-and-key-surfaces.md) (`config::env_file` + `config::env_key`)

Now in [§6c–§6d — Managed key file, onboarding & auxiliary models](heads/keys-and-aux-models.md#6c-managed-provider-key-env-file--adr-0073-configenv_file--configenv_key).

## 6d. Per-purpose auxiliary models & auto session titles — [ADR-0154](../adr/0154-per-purpose-auxiliary-models.md)/[ADR-0183](../adr/0183-narrate-purpose-and-per-user-aux-pins.md) (`aux_llm` + `config::aux_models` + `session_title` + `narrate`)

Now in [§6c–§6d — Managed key file, onboarding & auxiliary models](heads/keys-and-aux-models.md#6d-per-purpose-auxiliary-models--auto-session-titles--adr-0154adr-0183-aux_llm--configaux_models--session_title--narrate).

## 6b. Session persistence & resume — [ADR-0020](../adr/0020-event-sourced-session-persistence.md) (`persistence` + `session_store`)

Now in [§6b — Session persistence & resume](heads/persistence.md#6b-session-persistence--resume--adr-0020-persistence--session_store).
